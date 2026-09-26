//! `system-control-daemon` — resident `system_control` daemon over one
//! state root (`daemon` feature, Unix only).
//!
//! The binary is a thin shell around [`nlos_system_control::daemon`]: it
//! parses the options, assembles every real authority under `--root`, starts
//! the `TaskAuthorityCommitRecoveryWorker`, binds the two endpoints, and
//! serves them until SIGINT/SIGTERM. The desktop GUI connects to the
//! authenticated endpoint (ADR-0011 handshake); the `system-control-cli`
//! binary connects to the plain endpoint. See the library module
//! documentation for the exact assembly, fail-closed executor posture, and
//! the per-exchange handler construction.
//!
//! # Usage
//!
//! ```text
//! system-control-daemon --root <DIR> [--auth-socket <PATH>] [--plain-socket <PATH>] [--identity-key-file <PATH>]
//! ```
//!
//! - `--root` — state root; every authority is opened under it and both
//!   socket paths default into it (`system-control-auth.sock` /
//!   `system-control-plain.sock`).
//! - `--identity-key-file` — optional 0600 file holding one trimmed line of
//!   64 hex characters, the Ed25519 seed in the desktop client's key-file
//!   format. When given, the daemon bootstraps (idempotently, key-derived)
//!   the matching principal in `<root>/identity` so a client holding the
//!   same seed can authenticate. Without it the daemon serves whatever
//!   principals already exist in the identity authority.
//!
//! # Output and exit contract
//!
//! After both endpoints are bound the daemon prints one `READY` line with
//! both socket paths (and the bootstrapped principal, if any) for script
//! probing. SIGINT/SIGTERM stop the accept loops (an idle accept window is
//! bounded by the transport's 5s timeout), stop the recovery worker, remove
//! the socket files, print `STOPPED`, and exit 0. A startup failure prints
//! one typed error line and exits 2.

#[cfg(all(unix, feature = "daemon"))]
use std::path::PathBuf;
use std::process::ExitCode;
#[cfg(all(unix, feature = "daemon"))]
use std::sync::Arc;
#[cfg(all(unix, feature = "daemon"))]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(all(unix, feature = "daemon"))]
use nlos_system_control::daemon::{
    DaemonOptions, assemble, serve_authenticated_endpoint, serve_plain_endpoint,
};

#[cfg(all(unix, feature = "daemon"))]
const USAGE: &str = "usage: system-control-daemon --root <DIR> \
[--auth-socket <PATH>] [--plain-socket <PATH>] [--identity-key-file <PATH>]";

#[cfg(all(unix, feature = "daemon"))]
enum ParseFailure {
    Help,
    Error(&'static str),
}

#[cfg(all(unix, feature = "daemon"))]
struct ParsedArguments {
    options: DaemonOptions,
}

#[cfg(all(unix, feature = "daemon"))]
fn parsed_arguments() -> Result<ParsedArguments, ParseFailure> {
    let mut arguments = std::env::args().skip(1);
    let mut root: Option<PathBuf> = None;
    let mut auth_socket: Option<PathBuf> = None;
    let mut plain_socket: Option<PathBuf> = None;
    let mut identity_key_file: Option<PathBuf> = None;
    while let Some(flag) = arguments.next() {
        let mut value = || {
            arguments
                .next()
                .ok_or(ParseFailure::Error("missing value after flag"))
        };
        match flag.as_str() {
            "--help" | "-h" => return Err(ParseFailure::Help),
            "--root" => root = Some(PathBuf::from(value()?)),
            "--auth-socket" => auth_socket = Some(PathBuf::from(value()?)),
            "--plain-socket" => plain_socket = Some(PathBuf::from(value()?)),
            "--identity-key-file" => identity_key_file = Some(PathBuf::from(value()?)),
            _ => return Err(ParseFailure::Error("unknown flag")),
        }
    }
    let Some(root) = root else {
        return Err(ParseFailure::Error("--root is required"));
    };
    Ok(ParsedArguments {
        options: DaemonOptions {
            root,
            auth_socket,
            plain_socket,
            identity_key_file,
            worker_config: nlos_commit_coordinator::RecoveryWorkerConfig::default(),
        },
    })
}

#[cfg(all(unix, feature = "daemon"))]
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
        // A signal-handler installation failure must not strand the daemon;
        // falling back to Ctrl-C only is the narrowest degradation.
        (Err(error), _) | (_, Err(error)) => {
            eprintln!("system-control-daemon: signal handler unavailable: {error}");
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

#[cfg(all(unix, feature = "daemon"))]
#[tokio::main]
async fn main() -> ExitCode {
    let parsed = match parsed_arguments() {
        Ok(parsed) => parsed,
        Err(ParseFailure::Help) => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
        Err(ParseFailure::Error(reason)) => {
            eprintln!("system-control-daemon: {reason}");
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let (daemon, endpoints) = match assemble(parsed.options, tokio::runtime::Handle::current()) {
        Ok(assembled) => assembled,
        Err(error) => {
            eprintln!("system-control-daemon: assembly failed: {error}");
            return ExitCode::from(2);
        }
    };
    println!(
        "READY service=system_control root={} auth_socket={} plain_socket={} principal={}",
        daemon.root.display(),
        daemon.auth_socket_path.display(),
        daemon.plain_socket_path.display(),
        daemon
            .bootstrapped_principal_hex
            .as_deref()
            .unwrap_or("none"),
    );

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
    stop.store(true, Ordering::Relaxed);
    let _ = authenticated.await;
    let _ = plain.await;
    daemon.stop_worker();
    let _ = std::fs::remove_file(&daemon.auth_socket_path);
    let _ = std::fs::remove_file(&daemon.plain_socket_path);
    println!("STOPPED service=system_control");
    ExitCode::SUCCESS
}

#[cfg(not(all(unix, feature = "daemon")))]
fn main() -> ExitCode {
    eprintln!(
        "system-control-daemon: the resident daemon ships Unix socket endpoints only; \
         build it with --features daemon on a Unix host"
    );
    ExitCode::from(2)
}
