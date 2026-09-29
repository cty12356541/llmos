//! Resident daemon assembly tests (`daemon` feature): the real authority
//! assembly, both socket binds, the health face, worker start/stop, one
//! full round-trip per endpoint, and stale-path rebinding. No
//! long-running process is left behind — every test drives the loops
//! directly and stops or aborts them.

#![cfg(all(unix, feature = "daemon"))]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ed25519_dalek::Signer as _;
use nlos_commit_coordinator::RecoveryWorkerState;
use nlos_system_control::auth::dispatch_over_authenticated_socket;
use nlos_system_control::control::dispatch_over_socket;
use nlos_system_control::control::{ControlCommand, ControlOutcome, RecoveryWorkerLifecycle};
use nlos_system_control::daemon::{
    DaemonOptions, assemble, serve_authenticated_endpoint, serve_plain_endpoint,
};
use nlos_types::PrincipalId;

static NEXT_PATH: AtomicU64 = AtomicU64::new(0);

/// Short-lived temp root (sockets must stay under the macOS `SUN_LEN`
/// bound).
struct TempRoot(PathBuf);

impl TempRoot {
    fn new(label: &str) -> Self {
        let sequence = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "nlos-daemon-{label}-{}-{sequence}",
            std::process::id(),
        )))
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn temp_path(label: &str, suffix: &str) -> PathBuf {
    let sequence = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "nlos-daemon-{label}-{suffix}-{}-{sequence}.sock",
        std::process::id(),
    ))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn hex16(value: &str) -> [u8; 16] {
    let bytes = value.as_bytes();
    assert_eq!(bytes.len(), 32, "principal hex");
    let mut out = [0u8; 16];
    for (index, byte) in out.iter_mut().enumerate() {
        let hi = (bytes[2 * index] as char).to_digit(16).expect("hex");
        let lo = (bytes[2 * index + 1] as char).to_digit(16).expect("hex");
        *byte = u8::try_from(hi * 16 + lo).expect("byte");
    }
    out
}

fn worker_state(lifecycle: RecoveryWorkerLifecycle) -> RecoveryWorkerState {
    match lifecycle {
        RecoveryWorkerLifecycle::Starting => RecoveryWorkerState::Starting,
        RecoveryWorkerLifecycle::Running => RecoveryWorkerState::Running,
        RecoveryWorkerLifecycle::BackingOff => RecoveryWorkerState::BackingOff,
        RecoveryWorkerLifecycle::Faulted => RecoveryWorkerState::Faulted,
        RecoveryWorkerLifecycle::Stopped => RecoveryWorkerState::Stopped,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assembly_opens_authorities_binds_sockets_and_starts_the_worker() {
    let root = TempRoot::new("assemble");
    let auth_socket = temp_path("assemble", "auth");
    let plain_socket = temp_path("assemble", "plain");
    let options = DaemonOptions::new(&root.0)
        .with_auth_socket(&auth_socket)
        .with_plain_socket(&plain_socket);
    let (daemon, endpoints) =
        assemble(options, tokio::runtime::Handle::current()).expect("assemble daemon");

    // Both endpoints bound where the options say; the daemon reports the
    // same paths it prints in the READY line.
    assert!(auth_socket.exists(), "authenticated socket bound");
    assert!(plain_socket.exists(), "plain socket bound");
    assert_eq!(daemon.auth_socket_path, auth_socket);
    assert_eq!(daemon.plain_socket_path, plain_socket);
    assert_eq!(daemon.root, root.0);
    assert!(daemon.bootstrapped_principal_hex.is_none());

    // The health face is queryable and the worker is live.
    assert_ne!(daemon.recovery_health().state, RecoveryWorkerState::Stopped);

    // The in-process dispatch face serves the same handler the sockets
    // serve, with every layer source wired and the executor arms unwired.
    let receipt = daemon
        .dispatch_command(&ControlCommand::InspectHealth, 10, 6_000)
        .expect("dispatch");
    let ControlOutcome::Inspected(inspection) = receipt.outcome.expect("health snapshot") else {
        panic!("expected aggregate health inspection");
    };
    assert_eq!(
        worker_state(inspection.worker_state),
        daemon.recovery_health().state
    );

    let unwired = daemon
        .dispatch_command(
            &ControlCommand::PauseOperation {
                control_command_id: [0x51; 16],
                target_id: [0x52; 16],
                expected_generation_or_revision: 1,
                reason: "daemon keeps executor arms unwired".to_owned(),
            },
            10,
            6_000,
        )
        .expect("dispatch");
    let failure = unwired.outcome.expect_err("executor arms stay unwired");
    assert_eq!(
        failure.safe_message,
        "operation control execution backend is not wired"
    );

    // Worker stop is observable and idempotent.
    daemon.stop_worker();
    assert_eq!(daemon.recovery_health().state, RecoveryWorkerState::Stopped);
    daemon.stop_worker();
    drop(endpoints);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plain_endpoint_serves_the_shared_handler_path_and_stops_gracefully() {
    let root = TempRoot::new("plain");
    let socket_path = temp_path("plain", "plain");
    let options = DaemonOptions::new(&root.0).with_plain_socket(&socket_path);
    let (daemon, endpoints) =
        assemble(options, tokio::runtime::Handle::current()).expect("assemble daemon");
    let stop = Arc::new(AtomicBool::new(false));
    let server = tokio::spawn(serve_plain_endpoint(
        Arc::clone(&daemon),
        endpoints.listener_plain,
        Arc::clone(&stop),
    ));

    let receipt = dispatch_over_socket(
        &socket_path,
        &ControlCommand::InspectHealth,
        None,
        None,
        None,
    )
    .await
    .expect("plain round-trip");
    assert!(
        receipt.outcome.is_ok(),
        "health read served: {:?}",
        receipt.outcome
    );

    // Graceful stop: the loop observes the flag within one bounded accept
    // window (transport default: 5s) and retires.
    stop.store(true, Ordering::Relaxed);
    server.await.expect("plain loop retires after stop");

    daemon.stop_worker();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authenticated_endpoint_serves_a_bootstrapped_key_file_client() {
    let root = TempRoot::new("auth");
    let socket_path = temp_path("auth", "auth");

    // The desktop client's key-file format: one 0600 file, 64 hex seed.
    let seed = [0x7A; 32];
    fs::create_dir_all(&root.0).expect("create root");
    let key_file = root.0.join("client.key");
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&key_file)
            .expect("create key file");
        file.write_all(hex(&seed).as_bytes()).expect("write seed");
    }
    let options = DaemonOptions::new(&root.0)
        .with_auth_socket(&socket_path)
        .with_identity_key_file(&key_file);
    let (daemon, endpoints) =
        assemble(options, tokio::runtime::Handle::current()).expect("assemble daemon");
    let principal_hex = daemon
        .bootstrapped_principal_hex
        .clone()
        .expect("bootstrap reported the principal");
    let stop = Arc::new(AtomicBool::new(false));
    let server = tokio::spawn(serve_authenticated_endpoint(
        Arc::clone(&daemon),
        endpoints.listener_authenticated,
        Arc::clone(&stop),
    ));

    let key = ed25519_dalek::SigningKey::from_bytes(&seed);
    let receipt = dispatch_over_authenticated_socket(
        &socket_path,
        PrincipalId::from_bytes(hex16(&principal_hex)),
        |digest| Ok(key.sign(digest).to_bytes()),
        &ControlCommand::InspectHealth,
        None,
        None,
        None,
    )
    .await
    .expect("authenticated round-trip");
    assert!(
        receipt.outcome.is_ok(),
        "health read served: {:?}",
        receipt.outcome
    );

    stop.store(true, Ordering::Relaxed);
    server.await.expect("authenticated loop retires after stop");
    daemon.stop_worker();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_socket_paths_are_rebound_by_the_next_assembly() {
    let root = TempRoot::new("rebind");
    let auth_socket = temp_path("rebind", "auth");
    let plain_socket = temp_path("rebind", "plain");
    {
        let options = DaemonOptions::new(&root.0)
            .with_auth_socket(&auth_socket)
            .with_plain_socket(&plain_socket);
        let (_daemon, endpoints) =
            assemble(options, tokio::runtime::Handle::current()).expect("first assembly");
        drop(endpoints);
        // Simulate an unclean exit: the socket files stay behind.
        assert!(auth_socket.exists());
    }
    let options = DaemonOptions::new(&root.0)
        .with_auth_socket(&auth_socket)
        .with_plain_socket(&plain_socket);
    let (_daemon, endpoints) = assemble(options, tokio::runtime::Handle::current())
        .expect("second assembly rebinds over the stale path");
    assert!(auth_socket.exists());
    drop(endpoints);
}
