//! `nlos-up` — one-command local trial bootstrapper and cross-platform launcher.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use nlos_system_control::control::ControlCommand;
use nlos_system_control::daemon::{
    SystemControlDaemon, assemble, serve_authenticated_endpoint, serve_plain_endpoint,
};
use nlos_up::{
    BootstrapOptions, BootstrapReceipt, DesktopSessionEnv, KeyOrigin, LocalKeyPoolStatusReport,
    SampleAppSeedStatus, ShellKind, TrialCompiledIntentRequest, TrialMeteredChatRequest, UpError,
    bootstrap_trial_root, daemon_options_for_receipt, encode_hex, inspect_trial_keypool_status,
    resolve_desktop_env, run_trial_compiled_intent, run_trial_metered_chat,
};

const USAGE: &str = "\
usage:
  nlos-up init [--root <DIR>] [--seed-sample-app] [--sample-package-dir <DIR>] [--keypool-config <FILE>] [--no-keypool-probe]
  nlos-up env [--root <DIR>] [--shell powershell|posix] [--keypool-config <FILE>] [--no-keypool-probe]
  nlos-up daemon [--root <DIR>] [--seed-sample-app] [--sample-package-dir <DIR>] [--keypool-config <FILE>] [--no-keypool-probe]
  nlos-up desktop [--root <DIR>] [--desktop-dir <DIR>] [--dry-run] [--seed-sample-app] [--sample-package-dir <DIR>] [--keypool-config <FILE>] [--no-keypool-probe]
  nlos-up pool-status [--keypool-config <FILE>] [--require-live]
  nlos-up chat [--root <DIR>] [--keypool-config <FILE>] [--model <MODEL>] [--max-tokens <N>] [--session <ID>] --prompt <TEXT>
  nlos-up intent [--root <DIR>] [--keypool-config <FILE>] [--model <MODEL>] [--locale zh-CN|en-US] --query <TEXT>";

enum Command {
    Init(BootstrapOptions),
    Env {
        options: BootstrapOptions,
        shell: ShellKind,
    },
    Daemon(BootstrapOptions),
    Desktop {
        options: BootstrapOptions,
        desktop_dir: Option<PathBuf>,
        dry_run: bool,
    },
    PoolStatus {
        keypool_config: Option<PathBuf>,
        require_live: bool,
    },
    Chat(TrialMeteredChatRequest),
    Intent(TrialCompiledIntentRequest),
}

enum ParseOutcome {
    Help,
    Command(Command),
}

fn parse_cli_args<I>(mut args: I) -> Result<ParseOutcome, UpError>
where
    I: Iterator<Item = String>,
{
    let Some(subcommand) = args.next() else {
        return Ok(ParseOutcome::Help);
    };
    if matches!(subcommand.as_str(), "-h" | "--help" | "help") {
        return Ok(ParseOutcome::Help);
    }

    let mut options = BootstrapOptions::new();
    let mut shell = ShellKind::host_default();
    let mut desktop_dir: Option<PathBuf> = None;
    let mut dry_run = false;
    let mut require_live = false;
    let mut model: Option<String> = None;
    let mut max_tokens: Option<u32> = None;
    let mut session_id: Option<String> = None;
    let mut prompt: Option<String> = None;
    let mut query: Option<String> = None;
    let mut locale = "zh-CN".to_owned();

    while let Some(flag) = args.next() {
        let mut next_val = || {
            args.next().ok_or_else(|| {
                UpError::InvalidArgument(format!("missing value after flag '{flag}'"))
            })
        };
        match flag.as_str() {
            "-h" | "--help" => return Ok(ParseOutcome::Help),
            "--root" if subcommand != "pool-status" => options = options.with_root(next_val()?),
            "--seed-sample-app" if subcommand != "pool-status" => {
                options = options.with_seed_sample_app(true);
            }
            "--sample-package-dir" if subcommand != "pool-status" => {
                options = options.with_sample_package_dir(next_val()?);
            }
            "--keypool-config" => options = options.with_keypool_config(next_val()?),
            "--no-keypool-probe" if subcommand != "pool-status" => {
                options = options.without_default_keypool_probe();
            }
            "--shell" if subcommand == "env" => {
                shell = ShellKind::parse(&next_val()?)?;
            }
            "--desktop-dir" if subcommand == "desktop" => {
                desktop_dir = Some(PathBuf::from(next_val()?));
            }
            "--dry-run" if subcommand == "desktop" => {
                dry_run = true;
            }
            "--require-live" if subcommand == "pool-status" => {
                require_live = true;
            }
            "--model" if matches!(subcommand.as_str(), "chat" | "intent") => {
                model = Some(next_val()?);
            }
            "--max-tokens" if subcommand == "chat" => {
                let raw = next_val()?;
                let parsed = raw.parse::<u32>().map_err(|_| {
                    UpError::InvalidArgument(format!(
                        "--max-tokens expects a positive 32-bit integer, got '{raw}'"
                    ))
                })?;
                if parsed == 0 {
                    return Err(UpError::InvalidArgument(
                        "--max-tokens must be greater than zero".to_owned(),
                    ));
                }
                max_tokens = Some(parsed);
            }
            "--session" if subcommand == "chat" => {
                session_id = Some(next_val()?);
            }
            "--prompt" if subcommand == "chat" => {
                prompt = Some(next_val()?);
            }
            "--query" if subcommand == "intent" => {
                query = Some(next_val()?);
            }
            "--locale" if subcommand == "intent" => {
                locale = next_val()?;
            }
            other => {
                return Err(UpError::InvalidArgument(format!(
                    "unknown flag '{other}' for subcommand '{subcommand}'"
                )));
            }
        }
    }

    match subcommand.as_str() {
        "init" => Ok(ParseOutcome::Command(Command::Init(options))),
        "env" => Ok(ParseOutcome::Command(Command::Env { options, shell })),
        "daemon" => Ok(ParseOutcome::Command(Command::Daemon(options))),
        "desktop" => Ok(ParseOutcome::Command(Command::Desktop {
            options,
            desktop_dir,
            dry_run,
        })),
        "pool-status" => Ok(ParseOutcome::Command(Command::PoolStatus {
            keypool_config: options.keypool_config_override,
            require_live,
        })),
        "chat" => {
            let prompt = prompt.filter(|s| !s.trim().is_empty()).ok_or_else(|| {
                UpError::InvalidArgument("chat requires non-empty --prompt <TEXT>".to_owned())
            })?;
            Ok(ParseOutcome::Command(Command::Chat(
                TrialMeteredChatRequest {
                    options,
                    prompt,
                    model,
                    max_tokens,
                    session_id,
                },
            )))
        }
        "intent" => {
            let query = query.filter(|s| !s.trim().is_empty()).ok_or_else(|| {
                UpError::InvalidArgument("intent requires non-empty --query <TEXT>".to_owned())
            })?;
            Ok(ParseOutcome::Command(Command::Intent(
                TrialCompiledIntentRequest {
                    options,
                    query,
                    locale,
                    model,
                },
            )))
        }
        other => Err(UpError::InvalidArgument(format!(
            "unknown subcommand '{other}'"
        ))),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let parsed = match parse_cli_args(std::env::args().skip(1)) {
        Ok(ParseOutcome::Help) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(ParseOutcome::Command(command)) => command,
        Err(error) => {
            eprintln!("nlos-up: {error}");
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    let outcome = match parsed {
        Command::Init(options) => run_init(&options),
        Command::Env { options, shell } => run_env(&options, shell),
        Command::Daemon(options) => run_daemon(&options).await,
        Command::Desktop {
            options,
            desktop_dir,
            dry_run,
        } => run_desktop(&options, desktop_dir.as_deref(), dry_run).await,
        Command::PoolStatus {
            keypool_config,
            require_live,
        } => run_pool_status(keypool_config.as_deref(), require_live).await,
        Command::Chat(request) => run_chat(&request).await,
        Command::Intent(request) => run_intent(&request).await,
    };

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("nlos-up: {error}");
            ExitCode::from(1)
        }
    }
}

fn run_init(options: &BootstrapOptions) -> Result<(), UpError> {
    let receipt = bootstrap_trial_root(options)?;
    print_bootstrap_summary(&receipt);
    Ok(())
}

fn run_env(options: &BootstrapOptions, shell: ShellKind) -> Result<(), UpError> {
    let session_env = resolve_desktop_env(options)?;
    print!("{}", session_env.render_shell(shell));
    Ok(())
}

async fn run_pool_status(keypool_config: Option<&Path>, require_live: bool) -> Result<(), UpError> {
    let report = inspect_trial_keypool_status(keypool_config, require_live).await?;
    print_pool_status_report(&report);
    Ok(())
}

async fn run_chat(request: &TrialMeteredChatRequest) -> Result<(), UpError> {
    let report = run_trial_metered_chat(request).await?;
    println!("NLOS_UP_CHAT status=ok");
    println!(
        "trial_account           = {}",
        report.bootstrap.account_id_hex
    );
    println!("model                   = {}", report.outcome.model);
    println!("isolation_tag           = {}", report.outcome.isolation_tag);
    println!(
        "served_group            = {}",
        report.outcome.served_group.as_deref().unwrap_or("none")
    );
    println!(
        "served_slot_fingerprint = {}",
        report
            .outcome
            .served_slot_fingerprint
            .as_deref()
            .unwrap_or("none")
    );
    println!(
        "usage                   = prompt_tokens={} completion_tokens={} reasoning_tokens={} total_tokens={} estimated_fallback={}",
        report.outcome.usage.prompt_tokens,
        report.outcome.usage.completion_tokens,
        report.outcome.usage.reasoning_tokens,
        report.outcome.usage.total_tokens,
        report.outcome.usage.estimated_fallback
    );
    if let (Some(receipt), Some(receipt_hex)) = (
        report.outcome.consumption_receipt,
        report.receipt_id_hex.as_deref(),
    ) {
        println!("receipt_id              = {receipt_hex}");
        println!("sequence                = {}", receipt.sequence);
        println!(
            "cumulative_total_tokens = {}",
            receipt.cumulative_total_tokens
        );
        println!("cumulative_api_calls    = {}", receipt.cumulative_api_calls);
    }
    println!("--- reply ---");
    println!("{}", report.outcome.content);
    Ok(())
}

async fn run_intent(request: &TrialCompiledIntentRequest) -> Result<(), UpError> {
    let report = run_trial_compiled_intent(request).await?;
    println!("{}", report.compiled.to_json_value().to_json_pretty());
    Ok(())
}

fn print_pool_status_report(report: &LocalKeyPoolStatusReport) {
    println!(
        "NLOS_UP_POOL_STATUS configured={} reachable={}",
        report.configured, report.reachable
    );
    println!("discovery_source     = {}", report.discovery_source);
    println!(
        "gateway_origin       = {}",
        report.gateway_origin.as_deref().unwrap_or("none")
    );
    println!(
        "openai_base_path     = {}",
        report.openai_base_path.as_deref().unwrap_or("none")
    );
    println!(
        "openai_base_url      = {}",
        report.openai_base_url.as_deref().unwrap_or("none")
    );
    println!(
        "default_model        = {}",
        report.default_model.as_deref().unwrap_or("none")
    );
    println!(
        "fast_intent_model    = {}",
        report.fast_intent_model.as_deref().unwrap_or("none")
    );
    println!(
        "allowed_models       = [{}]",
        report.allowed_models.join(", ")
    );
    println!("configured_key_slots = {}", report.configured_key_slots);
    println!(
        "credential_present   = {} {}",
        report.credential_present, report.credential_marker
    );
    println!("isolation_tag        = {}", report.isolation_tag);
    if let Some(health) = &report.live_health {
        println!("pool_strategy        = {}", health.strategy);
        println!(
            "pool_keys            = {}/{}",
            health.active_keys, health.total_keys
        );
        println!("pool_active_sessions = {}", health.active_sessions);
        println!(
            "pool_requests        = total={} ok={} failed={}",
            health.total_requests, health.ok_requests, health.failed_requests
        );
        println!(
            "pool_tokens          = in={} out={}",
            health.tokens_in, health.tokens_out
        );
    } else if let Some(reason) = &report.unreachable_reason {
        println!("unreachable_reason   = {reason}");
    }
}

async fn run_daemon(options: &BootstrapOptions) -> Result<(), UpError> {
    let receipt = bootstrap_trial_root(options)?;
    let (daemon, endpoints) = assemble(
        daemon_options_for_receipt(&receipt),
        tokio::runtime::Handle::current(),
    )?;

    print_daemon_ready(&daemon, &receipt);
    let session_env = session_env_from_live_daemon(&daemon, &receipt);
    println!();
    println!("# ---- PowerShell session configuration ----");
    print!("{}", session_env.render_shell(ShellKind::PowerShell));
    println!("# ---- POSIX shell session configuration ----");
    print!("{}", session_env.render_shell(ShellKind::Posix));
    println!("# Press Ctrl+C to stop the resident daemon.");

    let stop = Arc::new(AtomicBool::new(false));
    let authenticated = tokio::spawn(serve_authenticated_endpoint(
        Arc::clone(&daemon),
        endpoints.listener_authenticated,
        Arc::clone(&stop),
    ));
    let plain = tokio::spawn(serve_plain_endpoint(
        Arc::clone(&daemon),
        endpoints.listener_plain,
        Arc::clone(&stop),
    ));

    wait_for_shutdown_signal().await;
    shutdown_embedded_daemon(&daemon, &stop, authenticated, plain).await;
    println!("STOPPED service=system_control");
    Ok(())
}

async fn run_desktop(
    options: &BootstrapOptions,
    explicit_desktop_dir: Option<&Path>,
    dry_run: bool,
) -> Result<(), UpError> {
    let desktop_dir = resolve_desktop_dir(explicit_desktop_dir)?;
    let receipt = bootstrap_trial_root(options)?;
    let (daemon, endpoints) = assemble(
        daemon_options_for_receipt(&receipt),
        tokio::runtime::Handle::current(),
    )?;

    // Probe in-process health once to verify all wired daemon authorities are live.
    daemon
        .dispatch_command(&ControlCommand::InspectHealth, 1_000_000, 0)
        .map_err(|error| UpError::DesktopProcess(format!("daemon health check failed: {error}")))?;

    let stop = Arc::new(AtomicBool::new(false));
    let authenticated = tokio::spawn(serve_authenticated_endpoint(
        Arc::clone(&daemon),
        endpoints.listener_authenticated,
        Arc::clone(&stop),
    ));
    let plain = tokio::spawn(serve_plain_endpoint(
        Arc::clone(&daemon),
        endpoints.listener_plain,
        Arc::clone(&stop),
    ));

    let session_env = session_env_from_live_daemon(&daemon, &receipt);
    print_daemon_ready(&daemon, &receipt);

    if dry_run {
        println!(
            "DRY_RUN desktop_dir={} command=\"npm run tauri dev\"",
            desktop_dir.display()
        );
        for (name, value) in session_env.entries() {
            println!("ENV {name}={value}");
        }
        shutdown_embedded_daemon(&daemon, &stop, authenticated, plain).await;
        println!("STOPPED service=system_control mode=dry-run");
        return Ok(());
    }

    let npm_program = if cfg!(windows) { "npm.cmd" } else { "npm" };
    let mut child = tokio::process::Command::new(npm_program)
        .args(["run", "tauri", "dev"])
        .current_dir(&desktop_dir)
        .envs(session_env.entries())
        .spawn()
        .map_err(|error| {
            UpError::DesktopProcess(format!(
                "failed to spawn '{npm_program} run tauri dev' in {}: {error}",
                desktop_dir.display()
            ))
        })?;

    let child_result = tokio::select! {
        status = child.wait() => Some(status),
        () = wait_for_shutdown_signal() => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            None
        }
    };

    shutdown_embedded_daemon(&daemon, &stop, authenticated, plain).await;
    println!("STOPPED service=system_control");

    if let Some(status_result) = child_result {
        let status = status_result.map_err(|error| {
            UpError::DesktopProcess(format!("failed waiting on desktop process: {error}"))
        })?;
        if !status.success() {
            return Err(UpError::DesktopProcess(format!(
                "'npm run tauri dev' exited with status {status}"
            )));
        }
    }
    Ok(())
}

fn session_env_from_live_daemon(
    daemon: &SystemControlDaemon,
    receipt: &BootstrapReceipt,
) -> DesktopSessionEnv {
    let mut env = DesktopSessionEnv::from_receipt(receipt);
    env.auth_socket_path.clone_from(&daemon.auth_socket_path);
    env.plain_socket_path.clone_from(&daemon.plain_socket_path);
    if let Some(principal) = &daemon.bootstrapped_principal_hex {
        env.principal_hex.clone_from(principal);
    }
    env
}

async fn shutdown_embedded_daemon(
    daemon: &SystemControlDaemon,
    stop: &AtomicBool,
    authenticated: tokio::task::JoinHandle<()>,
    plain: tokio::task::JoinHandle<()>,
) {
    stop.store(true, Ordering::Relaxed);
    authenticated.abort();
    plain.abort();
    let _ = authenticated.await;
    let _ = plain.await;
    daemon.stop_worker();
    daemon.stop_materialization();
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(&daemon.auth_socket_path);
        let _ = std::fs::remove_file(&daemon.plain_socket_path);
    }
}

fn resolve_desktop_dir(explicit: Option<&Path>) -> Result<PathBuf, UpError> {
    if let Some(dir) = explicit {
        if dir.is_dir() {
            return Ok(dir.to_path_buf());
        }
        return Err(UpError::InvalidArgument(format!(
            "desktop directory does not exist: {}",
            dir.display()
        )));
    }
    if let Ok(cwd) = std::env::current_dir() {
        let cwd_desktop = cwd.join("desktop");
        if cwd_desktop.join("package.json").is_file() {
            return Ok(cwd_desktop);
        }
        if cwd.join("src-tauri").is_dir() && cwd.join("package.json").is_file() {
            return Ok(cwd);
        }
    }
    let repo_desktop = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../desktop");
    if repo_desktop.is_dir() {
        return Ok(repo_desktop);
    }
    Err(UpError::InvalidArgument(
        "unable to locate desktop/ directory; pass --desktop-dir <DIR>".to_owned(),
    ))
}

fn print_bootstrap_summary(receipt: &BootstrapReceipt) {
    let key_origin = match receipt.key_origin {
        KeyOrigin::GeneratedDevTrial => "generated (dev-trial-only; NOT for production custody)",
        KeyOrigin::ReusedExisting => "reused-existing (dev-trial-only)",
    };
    println!("NLOS_UP_INIT status=ok");
    println!("root             = {}", receipt.root.display());
    println!(
        "operator_key     = {} [{key_origin}]",
        receipt.key_file.display()
    );
    println!("public_key       = {}", receipt.public_key_hex);
    println!(
        "principal        = {} (replayed={})",
        receipt.principal_hex, receipt.principal_replayed
    );
    println!(
        "trial_account    = {} (initial_credit={}, max_prompt_tokens={}, max_completion_tokens={}, max_total_tokens={}, max_api_calls={}, max_cost_microcredits={}, replayed={})",
        receipt.account_id_hex,
        receipt.account_record.initial_credit,
        receipt.llm_budget.limits.max_prompt_tokens,
        receipt.llm_budget.limits.max_completion_tokens,
        receipt.llm_budget.limits.max_total_tokens,
        receipt.llm_budget.limits.max_api_calls,
        receipt.llm_budget.limits.max_cost_microcredits,
        receipt.llm_budget_replayed,
    );
    println!("auth_socket      = {}", receipt.auth_socket_path.display());
    println!("plain_socket     = {}", receipt.plain_socket_path.display());
    match &receipt.sample_app {
        SampleAppSeedStatus::Skipped => println!("sample_app       = skipped"),
        SampleAppSeedStatus::Installed {
            package_id_hex,
            application_id_hex,
        } => println!(
            "sample_app       = installed (package_id={package_id_hex}, application_id={application_id_hex})"
        ),
        SampleAppSeedStatus::AlreadyInstalled {
            package_id_hex,
            application_id_hex,
        } => println!(
            "sample_app       = already-installed (package_id={package_id_hex}, application_id={application_id_hex})"
        ),
    }
    match &receipt.keypool {
        Some(probe) => println!(
            "glm_keypool      = detected (config={}, endpoint={})",
            probe.config_path.display(),
            probe.endpoint_url
        ),
        None => println!("glm_keypool      = not-detected"),
    }
}

fn print_daemon_ready(daemon: &SystemControlDaemon, receipt: &BootstrapReceipt) {
    let operator_root_receipt = daemon.operator_root.as_ref().map_or_else(
        || "none".to_owned(),
        |issuance| encode_hex(issuance.receipt.receipt_id.as_bytes()),
    );
    println!(
        "READY service=system_control root={} auth_socket={} plain_socket={} principal={} trial_account={} operator_root_receipt={}",
        daemon.root.display(),
        daemon.auth_socket_path.display(),
        daemon.plain_socket_path.display(),
        daemon
            .bootstrapped_principal_hex
            .as_deref()
            .unwrap_or(&receipt.principal_hex),
        receipt.account_id_hex,
        operator_root_receipt,
    );
}

#[cfg(unix)]
async fn wait_for_shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let interrupt = signal(SignalKind::interrupt());
    let terminate = signal(SignalKind::terminate());
    match (interrupt, terminate) {
        (Ok(mut interrupt), Ok(mut terminate)) => {
            tokio::select! {
                _ = interrupt.recv() => {},
                _ = terminate.recv() => {},
            }
        }
        (Err(error), _) | (_, Err(error)) => {
            eprintln!("nlos-up: unix signal handler unavailable: {error}");
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

#[cfg(windows)]
async fn wait_for_shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
